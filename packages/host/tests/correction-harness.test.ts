import { describe, expect, it } from "bun:test";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";
import type { AiChatRequest, AiProviderClient, AiTool } from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
import {
  TurnRunner,
  buildCorrectionMessages,
  resolveMaxCorrectionPasses,
  shouldRunCorrectionPass,
  type AppendMessageInput,
  type ContextProvider,
  type CorrectionConfig,
  type DeficiencyReport,
  type MessageRecord,
  type ToolSetContributor,
  type UsageAuthorizationDecision,
  type UsageAuthorizationRequest,
  type UsageAuthorizer,
  type UsageRecord,
  type VerificationHook,
  type VerificationInput,
} from "../src/index.js";
import { createHarness, type TestHarness } from "./fakes.js";

/** A provider client that remembers the history each call was handed. */
class RecordingClient implements AiProviderClient {
  readonly id = "test";
  readonly kind = "openai-compatible";
  readonly messagesPerCall: AiChatMessage[][] = [];
  /** How many tools each call was offered — 0 once a chat degrades to chat-only. */
  readonly toolCountsPerCall: number[] = [];
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
    this.messagesPerCall.push([...input.messages]);
    this.toolCountsPerCall.push(input.tools?.length ?? 0);
    yield* this.inner.streamChat(input);
  }
}

/**
 * A verifier that answers from a script and counts what it was asked.
 *
 * `"throw"` is a scripted answer of its own: "the checks could not run" is the
 * case the harness has to fail closed on, and a hook that cannot fail is a hook
 * that cannot test that.
 */
class ScriptedVerification implements VerificationHook {
  readonly calls: VerificationInput[] = [];

  constructor(
    private readonly answers: (DeficiencyReport | "throw" | null)[],
  ) {}

  async verify(input: VerificationInput): Promise<DeficiencyReport | null> {
    this.calls.push(input);
    const answer = this.answers[this.calls.length - 1] ?? null;
    if (answer === "throw") throw new Error("verifier is down");
    return answer;
  }
}

class RecordingUsageAuthorizer implements UsageAuthorizer {
  readonly authorizeCalls: UsageAuthorizationRequest[] = [];
  readonly records: UsageRecord[] = [];

  async authorize(
    request: UsageAuthorizationRequest,
  ): Promise<UsageAuthorizationDecision> {
    this.authorizeCalls.push(request);
    return { allowed: true };
  }

  async record(usage: UsageRecord): Promise<void> {
    this.records.push(usage);
  }
}

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
    const echoed = input.text ?? "";
    return {
      ok: true,
      data: { echoed },
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

const partial = (deficiencies: string[]): DeficiencyReport => ({
  status: "partial",
  checks: deficiencies.map((line, index) => ({
    id: `c${index}`,
    ok: false,
    message: line,
  })),
  deficiencies,
});

const clean: DeficiencyReport = {
  status: "pass",
  checks: [],
  deficiencies: [],
};

interface Fixture extends TestHarness {
  runner: TurnRunner;
  client: RecordingClient;
  mock: MockProviderClient;
  chatId: string;
  /** Every `appendMessage` this run made, in order — inputs, not records. */
  appends: AppendMessageInput[];
}

async function setup(
  options: {
    verification?: VerificationHook;
    correction?: CorrectionConfig;
    usage?: UsageAuthorizer;
    systemPrompt?: string;
    emitUsage?: boolean;
  } = {},
): Promise<Fixture> {
  const harness = createHarness();
  const mock = new MockProviderClient();
  if (options.emitUsage) mock.emitUsage = true;
  const client = new RecordingClient(mock);
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

  const appends: AppendMessageInput[] = [];
  const conversations = harness.store.conversations;
  const append = conversations.appendMessage.bind(conversations);
  conversations.appendMessage = async (input: AppendMessageInput) => {
    appends.push(input);
    return append(input);
  };

  const context: ContextProvider = {
    listBindings: async () => [],
    systemPrompt: async () => options.systemPrompt ?? null,
  };

  const runner = new TurnRunner({
    store: harness.store,
    taskRunner: harness.taskRunner,
    providerFactory: () => client,
    contributors: [echoContributor],
    clock: harness.clock,
    ids: harness.ids,
    context,
    ...(options.verification === undefined
      ? {}
      : { verification: options.verification }),
    ...(options.correction === undefined
      ? {}
      : { correction: options.correction }),
    ...(options.usage === undefined ? {} : { usage: options.usage }),
  });
  return { ...harness, runner, client, mock, chatId: chat.id, appends };
}

/** Stand in for the task runner: create the attempt, take the lease, execute. */
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

/**
 * A turn that calls one tool and then answers, followed by `corrections`
 * text-only turns — one per correction pass the harness is expected to run.
 */
function scriptToolThen(mock: MockProviderClient, corrections: string[]): void {
  mock.setScript([
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
    ...corrections.map((content) => ({
      steps: [{ kind: "text" as const, content }],
    })),
  ]);
}

async function run(f: Fixture): Promise<string> {
  const submitted = await f.runner.submitMessage({
    chatId: f.chatId,
    content: "go",
  });
  await drive(f, submitted.runId);
  return submitted.runId;
}

async function verificationEvents(
  f: Fixture,
  taskId: string,
): Promise<{ pass: number; status: string; deficiencies: string[] }[]> {
  const events = (await f.store.tasks.listEvents(taskId)) as AiRunEvent[];
  return events
    .filter((event) => event.type === "run.verification")
    .map((event) => event.data);
}

function messagesOf(f: Fixture): MessageRecord[] {
  return f.store.conversations.messages;
}

describe("correction harness — the stopping rule", () => {
  it("allows the first correction pass, which has nothing to compare against", () => {
    expect(
      shouldRunCorrectionPass({
        status: "partial",
        deficiencies: ["a", "b"],
        previousDeficiencies: undefined,
        passesRun: 0,
        maxPasses: 3,
      }),
    ).toBe(true);
  });

  it("continues only while the deficiency list is strictly shrinking", () => {
    const base = { status: "partial" as const, passesRun: 1, maxPasses: 3 };
    // Shorter: the last pass bought something.
    expect(
      shouldRunCorrectionPass({
        ...base,
        deficiencies: ["a"],
        previousDeficiencies: ["a", "b"],
      }),
    ).toBe(true);
    // Same length — even with entirely different lines — is a stall.
    expect(
      shouldRunCorrectionPass({
        ...base,
        deficiencies: ["c", "d"],
        previousDeficiencies: ["a", "b"],
      }),
    ).toBe(false);
    // Growing is worse than a stall.
    expect(
      shouldRunCorrectionPass({
        ...base,
        deficiencies: ["a", "b", "c"],
        previousDeficiencies: ["a", "b"],
      }),
    ).toBe(false);
  });

  it("stops on a pass, on an empty list, and at the cap", () => {
    expect(
      shouldRunCorrectionPass({
        status: "pass",
        deficiencies: [],
        previousDeficiencies: undefined,
        passesRun: 0,
        maxPasses: 3,
      }),
    ).toBe(false);
    expect(
      shouldRunCorrectionPass({
        status: "partial",
        deficiencies: [],
        previousDeficiencies: undefined,
        passesRun: 0,
        maxPasses: 3,
      }),
    ).toBe(false);
    expect(
      shouldRunCorrectionPass({
        status: "partial",
        deficiencies: ["a"],
        previousDeficiencies: ["a", "b"],
        passesRun: 3,
        maxPasses: 3,
      }),
    ).toBe(false);
  });

  it("clamps maxPasses to a sane ceiling and floors it at zero", () => {
    expect(resolveMaxCorrectionPasses({})).toBe(3);
    expect(resolveMaxCorrectionPasses({ maxPasses: 1 })).toBe(1);
    expect(resolveMaxCorrectionPasses({ maxPasses: 100 })).toBe(5);
    expect(resolveMaxCorrectionPasses({ maxPasses: 0 })).toBe(0);
    expect(resolveMaxCorrectionPasses({ maxPasses: -2 })).toBe(0);
  });

  it("omits the assistant turn when the previous pass said nothing", () => {
    expect(
      buildCorrectionMessages({
        systemPrompt: null,
        userRequest: null,
        previousContent: "   ",
        writeBack: "fix it",
      }).map((m) => m.role),
    ).toEqual(["user"]);
  });

  // F-OWN-5: the minimal re-context used to omit the one thing the model is
  // being asked to correct AGAINST — what was asked for.
  it("puts the originating request between the system prompt and the previous answer", () => {
    expect(
      buildCorrectionMessages({
        systemPrompt: "you are an EDA assistant",
        userRequest: "add decoupling caps to U3",
        previousContent: "done, added them everywhere",
        writeBack: "fix it",
      }),
    ).toEqual([
      { role: "system", content: "you are an EDA assistant" },
      { role: "user", content: "add decoupling caps to U3" },
      { role: "assistant", content: "done, added them everywhere" },
      { role: "user", content: "fix it" },
    ]);
  });

  it("omits a blank request the same way it omits a blank answer", () => {
    expect(
      buildCorrectionMessages({
        systemPrompt: null,
        userRequest: "   ",
        previousContent: "an answer",
        writeBack: "fix it",
      }).map((m) => m.role),
    ).toEqual(["assistant", "user"]);
  });
});

describe("TurnRunner + correction harness", () => {
  it("feeds the deficiencies back and stops when the next pass verifies clean", async () => {
    const verification = new ScriptedVerification([
      partial(["two items still unlinked", "one footprint missing"]),
      clean,
    ]);
    const f = await setup({ verification, correction: { maxPasses: 3 } });
    scriptToolThen(f.mock, ["fixed both"]);
    const runId = await run(f);

    expect(await verificationEvents(f, runId)).toEqual([
      {
        pass: 0,
        status: "partial",
        deficiencies: ["two items still unlinked", "one footprint missing"],
      },
      { pass: 1, status: "pass", deficiencies: [] },
    ]);
    expect(verification.calls.length).toBe(2);
    // Two provider calls for the run, one more for the correction pass.
    expect(f.client.calls).toBe(3);
    expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");

    // The write-back is a CHAIN append: internal, off the run's own last write,
    // never activating a path a user may have moved away from.
    const writeBack = f.appends.find(
      (input) => input.metadata?.["internal"] === true && input.role === "user",
    );
    expect(writeBack).toBeDefined();
    expect(writeBack?.activate).toBe(false);
    expect(typeof writeBack?.parentMessageId).toBe("string");
    expect(writeBack?.content).toContain("two items still unlinked");
    expect(writeBack?.content).toContain("one footprint missing");
    expect(writeBack?.runId).toBe(runId);

    // Verified clean in the end: no banner is posted about problems that are gone.
    expect(
      messagesOf(f).some((m) => m.metadata["banner"] === "verification"),
    ).toBe(false);
    // The corrected answer is what the reader is left with.
    const assistant = messagesOf(f).find(
      (m) => m.role === "assistant" && m.metadata["placeholder"] === false,
    );
    expect(assistant?.content).toBe("fixed both");
  });

  it("stops on a stall — the same deficiencies twice — well short of the cap", async () => {
    const verification = new ScriptedVerification([
      partial(["x", "y"]),
      partial(["x", "y"]),
      partial(["x"]),
    ]);
    const f = await setup({ verification, correction: { maxPasses: 3 } });
    scriptToolThen(f.mock, ["tried", "tried again", "tried once more"]);
    const runId = await run(f);

    expect(verification.calls.length).toBe(2);
    expect(await verificationEvents(f, runId)).toEqual([
      { pass: 0, status: "partial", deficiencies: ["x", "y"] },
      { pass: 1, status: "partial", deficiencies: ["x", "y"] },
    ]);
    // One correction pass only, despite maxPasses 3 and a script ready for more.
    expect(f.client.calls).toBe(3);
    // A partial verification does not fail the run.
    expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
    const banner = messagesOf(f).find(
      (m) => m.metadata["banner"] === "verification",
    );
    expect(banner?.metadata["status"]).toBe("partial");
    expect(banner?.content).toContain("- x");
  });

  it("stops at the cap while the deficiencies are still shrinking", async () => {
    const verification = new ScriptedVerification([
      partial(["a", "b", "c"]),
      partial(["a", "b"]),
      partial(["a"]),
      clean,
    ]);
    const f = await setup({ verification, correction: { maxPasses: 2 } });
    scriptToolThen(f.mock, ["one", "two", "three"]);
    const runId = await run(f);

    // Three verifications, two correction passes: the cap bites while progress
    // is still being made, which is the whole point of it being a cap.
    expect(verification.calls.length).toBe(3);
    expect(await verificationEvents(f, runId)).toEqual([
      { pass: 0, status: "partial", deficiencies: ["a", "b", "c"] },
      { pass: 1, status: "partial", deficiencies: ["a", "b"] },
      { pass: 2, status: "partial", deficiencies: ["a"] },
    ]);
    expect(f.client.calls).toBe(4);
    expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
  });

  it("fails closed when the verifier throws mid-harness", async () => {
    const verification = new ScriptedVerification([
      partial(["x", "y"]),
      "throw",
      clean,
    ]);
    const f = await setup({ verification, correction: { maxPasses: 3 } });
    scriptToolThen(f.mock, ["tried", "tried again"]);
    const runId = await run(f);

    expect(await verificationEvents(f, runId)).toEqual([
      { pass: 0, status: "partial", deficiencies: ["x", "y"] },
      { pass: 1, status: "unavailable", deficiencies: [] },
    ]);
    // Unavailable is never a pass, and never another pass either.
    expect(verification.calls.length).toBe(2);
    expect(f.client.calls).toBe(3);
    // A broken verifier does not take down a run that already answered.
    expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
  });

  it("treats a null report mid-harness as unavailable too", async () => {
    const verification = new ScriptedVerification([partial(["x", "y"]), null]);
    const f = await setup({ verification, correction: { maxPasses: 3 } });
    scriptToolThen(f.mock, ["tried"]);
    const runId = await run(f);

    expect(await verificationEvents(f, runId)).toEqual([
      { pass: 0, status: "partial", deficiencies: ["x", "y"] },
      { pass: 1, status: "unavailable", deficiencies: [] },
    ]);
    expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
  });

  it("keeps the superseded answer when a correction pass says nothing", async () => {
    const verification = new ScriptedVerification([partial(["x"]), clean]);
    const f = await setup({ verification, correction: { maxPasses: 1 } });
    // The correction turn is silent: tools only, no words.
    scriptToolThen(f.mock, [""]);
    await run(f);

    const assistant = messagesOf(f).find(
      (m) => m.role === "assistant" && m.metadata["placeholder"] === false,
    );
    expect(assistant?.content).toBe("all set");
  });

  it("sends MINIMAL re-context — system, the request, the last answer, the write-back", async () => {
    const verification = new ScriptedVerification([partial(["x", "y"]), clean]);
    const f = await setup({
      verification,
      correction: { maxPasses: 3 },
      systemPrompt: "SYSTEM PROMPT",
    });
    scriptToolThen(f.mock, ["fixed"]);
    await run(f);

    // The run's own first pass replays the conversation, as it always has.
    const initial = f.client.messagesPerCall[0] ?? [];
    expect(initial.map((m) => m.role)).toEqual(["system", "user"]);
    expect(initial.at(-1)?.content).toBe("go");

    const correction = f.client.messagesPerCall[2] ?? [];
    expect(correction.map((m) => m.role)).toEqual([
      "system",
      "user",
      "assistant",
      "user",
    ]);
    expect(correction[0]?.content).toBe("SYSTEM PROMPT");
    // F-OWN-5: the originating request, so the model correcting its work knows
    // what was asked. Four messages, not three — still nothing like the full
    // history.
    expect(correction[1]?.content).toBe("go");
    expect(correction[2]?.content).toBe("all set");
    const writeBack = String(correction[3]?.content ?? "");
    expect(writeBack).toContain("- x");
    expect(writeBack).toContain("- y");
    // NOT the full history: no tool turns, no tool results.
    expect(correction.some((m) => m.role === "tool")).toBe(false);
    expect(
      correction.some(
        (m) => m.toolCalls !== undefined && m.toolCalls.length > 0,
      ),
    ).toBe(false);
  });

  it("omits the request when includeUserRequest is off", async () => {
    const verification = new ScriptedVerification([partial(["x", "y"]), clean]);
    const f = await setup({
      verification,
      correction: { maxPasses: 3, includeUserRequest: false },
      systemPrompt: "SYSTEM PROMPT",
    });
    scriptToolThen(f.mock, ["fixed"]);
    await run(f);

    const correction = f.client.messagesPerCall[2] ?? [];
    expect(correction.map((m) => m.role)).toEqual([
      "system",
      "assistant",
      "user",
    ]);
    expect(correction.some((m) => m.content === "go")).toBe(false);
  });

  it("asks the usage authorizer once per pass, corrections included", async () => {
    const verification = new ScriptedVerification([
      partial(["a", "b"]),
      partial(["a"]),
      clean,
    ]);
    const usage = new RecordingUsageAuthorizer();
    const f = await setup({
      verification,
      correction: { maxPasses: 3 },
      usage,
      emitUsage: true,
    });
    scriptToolThen(f.mock, ["one", "two"]);
    await run(f);

    // One authorization for the run's own pass plus one per correction pass —
    // a correction is a provider call and bills like one.
    expect(usage.authorizeCalls.length).toBe(3);
    expect(usage.authorizeCalls.every((call) => call.model === "m1")).toBe(
      true,
    );
    // Every provider round-trip's usage is recorded, corrections included.
    expect(usage.records.length).toBe(f.client.calls);
  });
});

describe("TurnRunner without the correction harness", () => {
  it("verifies exactly once and writes no run.verification event", async () => {
    const verification = new ScriptedVerification([
      partial(["still unlinked"]),
      clean,
    ]);
    const f = await setup({ verification });
    scriptToolThen(f.mock, ["never asked for"]);
    const runId = await run(f);

    expect(verification.calls.length).toBe(1);
    expect(await verificationEvents(f, runId)).toEqual([]);
    // No correction pass was run: the provider saw only the run's own two calls.
    expect(f.client.calls).toBe(2);
    const banner = messagesOf(f).find(
      (m) => m.metadata["banner"] === "verification",
    );
    expect(banner?.content).toContain("still unlinked");
  });

  it("does nothing when correction is configured but no verifier is wired", async () => {
    const f = await setup({ correction: { maxPasses: 3 } });
    scriptToolThen(f.mock, ["never asked for"]);
    const runId = await run(f);

    expect(await verificationEvents(f, runId)).toEqual([]);
    expect(f.client.calls).toBe(2);
    expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
    expect(messagesOf(f).some((m) => m.role === "system")).toBe(false);
  });
});

/**
 * A turn that calls one tool and then answers, followed by correction passes
 * that CALL A TOOL OF THEIR OWN before answering, then `later` plain turns.
 *
 * This is the shape the harness produces in the field — its write-back tells the
 * model to "fix each of these now by calling your tools" — and it is the shape
 * that puts TWO tool-calling assistant turns under ONE run id.
 */
function scriptToolThenToolCorrections(
  mock: MockProviderClient,
  corrections: string[],
  later: string[] = [],
): void {
  mock.setScript([
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
    ...corrections.flatMap((content, index) => [
      {
        steps: [
          {
            kind: "tool_call" as const,
            toolCallId: `fix-${index + 1}`,
            name: "echo",
            argumentsJson: '{"text":"fixing"}',
          },
        ],
      },
      { steps: [{ kind: "text" as const, content }] },
    ]),
    ...later.map((content) => ({
      steps: [{ kind: "text" as const, content }],
    })),
  ]);
}

/**
 * Assert the one shape every provider enforces on a replayed history: an
 * assistant turn declaring `tool_calls` is followed IMMEDIATELY by exactly the
 * results for its own ids, in declaration order — so no two tool-calling
 * assistant turns can sit back to back, and no tool result can precede the turn
 * that asked for it.
 */
function assertProviderLegal(messages: readonly AiChatMessage[]): void {
  const declared = new Set<string>();
  for (const [index, message] of messages.entries()) {
    if (message.role === "tool") {
      // A result may only answer a call already declared above it.
      expect({
        at: index,
        id: message.toolCallId,
        declaredAbove: declared.has(String(message.toolCallId)),
      }).toEqual({ at: index, id: message.toolCallId, declaredAbove: true });
      continue;
    }
    const calls = message.toolCalls ?? [];
    if (message.role !== "assistant" || calls.length === 0) continue;
    for (const call of calls) declared.add(call.id);
    const expected = calls.map((call) => call.id);
    const actual = messages
      .slice(index + 1, index + 1 + expected.length)
      .map((next) =>
        next.role === "tool" ? next.toolCallId : `<${next.role}>`,
      );
    expect({ at: index, follows: actual }).toEqual({
      at: index,
      follows: expected,
    });
  }
}

/** The provider history the LAST turn replayed, before any recovery pass. */
function replayOf(f: Fixture, callIndex: number): AiChatMessage[] {
  return f.client.messagesPerCall[callIndex] ?? [];
}

async function submit(f: Fixture, content: string): Promise<string> {
  const submitted = await f.runner.submitMessage({ chatId: f.chatId, content });
  await drive(f, submitted.runId);
  return submitted.runId;
}

/** A record shaped as the provider message a replay of the fork would send. */
function asChatMessage(record: MessageRecord): AiChatMessage {
  return {
    role: record.role as AiChatMessage["role"],
    content: String(record.content),
    ...(record.toolCalls === undefined ? {} : { toolCalls: record.toolCalls }),
    ...(record.toolCallId === undefined
      ? {}
      : { toolCallId: record.toolCallId }),
  } as AiChatMessage;
}

describe("provider history after a multi-pass run", () => {
  it("replays each tool-calling pass next to its own results", async () => {
    const verification = new ScriptedVerification([partial(["a", "b"]), clean]);
    const f = await setup({ verification, correction: { maxPasses: 3 } });
    scriptToolThenToolCorrections(f.mock, ["fixed"], ["second answer"]);
    await submit(f, "go");
    // A SECOND turn is where the defect surfaces: the first turn assembles a
    // history that has none of its own records in it yet.
    await submit(f, "and again");

    // Call 4 is the second turn's own pass — the one handed the stored history.
    const replay = replayOf(f, 4);
    expect(f.client.calls).toBe(5);
    assertProviderLegal(replay);
    // Both passes really are in there: a legality check over a history that
    // quietly lost the second pass would pass for the wrong reason.
    const withCalls = replay.filter(
      (m) => m.role === "assistant" && (m.toolCalls ?? []).length > 0,
    );
    expect(withCalls.map((m) => (m.toolCalls ?? []).map((c) => c.id))).toEqual([
      ["call-1"],
      ["fix-1"],
    ]);
    expect(
      replay.filter((m) => m.role === "tool").map((m) => m.toolCallId),
    ).toEqual(["call-1", "fix-1"]);
    // The chat never degraded to chat-only, which is what a rejected history
    // costs: the second turn was still offered its tools.
    expect(f.client.toolCountsPerCall[4]).toBeGreaterThan(0);
  });

  it("stays legal across three tool-calling correction passes", async () => {
    const verification = new ScriptedVerification([
      partial(["a", "b", "c"]),
      partial(["a", "b"]),
      partial(["a"]),
      clean,
    ]);
    const f = await setup({ verification, correction: { maxPasses: 3 } });
    scriptToolThenToolCorrections(
      f.mock,
      ["one", "two", "three"],
      ["second answer"],
    );
    await submit(f, "go");
    await submit(f, "and again");

    const replay = replayOf(f, 8);
    assertProviderLegal(replay);
    expect(
      replay.filter((m) => m.role === "tool").map((m) => m.toolCallId),
    ).toEqual(["call-1", "fix-1", "fix-2", "fix-3"]);
  });

  it("keeps a fork of a corrected run provider-legal", async () => {
    const verification = new ScriptedVerification([partial(["a", "b"]), clean]);
    const f = await setup({ verification, correction: { maxPasses: 3 } });
    scriptToolThenToolCorrections(f.mock, ["fixed"]);
    await submit(f, "go");

    const leaf = (await f.store.conversations.listMessages(f.chatId)).at(-1);
    expect(leaf).toBeDefined();
    const fork = await f.store.conversations.forkChat(
      f.chatId,
      String(leaf?.id),
    );
    // The fork's STORED order is what it replays forever: `runId` does not
    // survive the copy, so nothing can repair the order afterwards.
    assertProviderLegal(fork.messages.map(asChatMessage));
    expect(
      fork.messages
        .filter((m) => m.role === "tool" || (m.toolCalls ?? []).length > 0)
        .map((m) => m.toolCallId ?? (m.toolCalls ?? []).map((c) => c.id)[0]),
    ).toEqual(["call-1", "call-1", "fix-1", "fix-1"]);
  });

  it("does not replay the correction write-back on a later turn", async () => {
    const verification = new ScriptedVerification([
      partial(["two items still unlinked"]),
      clean,
    ]);
    const f = await setup({ verification, correction: { maxPasses: 3 } });
    scriptToolThenToolCorrections(f.mock, ["fixed"], ["second answer"]);
    const runId = await submit(f, "go");
    await submit(f, "and again");

    const replay = replayOf(f, 4);
    expect(
      replay.some((m) =>
        String(m.content).includes("two items still unlinked"),
      ),
    ).toBe(false);
    expect(
      replay.some((m) => String(m.content).includes("Fix each of these now")),
    ).toBe(false);
    // Still on the record, though — the audit trail is why it is written.
    const stored = messagesOf(f).filter(
      (m) => m.runId === runId && m.metadata["correctionPass"] !== undefined,
    );
    expect(stored.length).toBe(1);
    expect(String(stored[0]?.content)).toContain("two items still unlinked");
  });
});

describe("multi-pass ordering meets orphan reconciliation", () => {
  it("answers a correction pass's lost tool result next to ITS OWN call", async () => {
    const verification = new ScriptedVerification([partial(["a", "b"]), clean]);
    const f = await setup({ verification, correction: { maxPasses: 3 } });
    scriptToolThenToolCorrections(f.mock, ["fixed"], ["second answer"]);
    await submit(f, "go");

    // The crash `reconcileOrphanToolCalls` exists for: the projection wrote the
    // correction pass's assistant turn and died before the result. Drop the
    // record the same way, from the leaf, so nothing is chained off it.
    const store = f.store.conversations;
    const lost = store.messages.findIndex((m) => m.toolCallId === "fix-1");
    expect(lost).toBeGreaterThan(-1);
    store.messages.splice(lost, 1);

    await submit(f, "and again");
    const replay = replayOf(f, 4);
    assertProviderLegal(replay);
    // The synthetic answer sits under the call it answers — the SECOND pass's —
    // not appended after the first pass's real result.
    const synthetic = replay.find((m) => m.toolCallId === "fix-1");
    expect(String(synthetic?.content)).toContain("tool_result_missing");
    expect(replay.indexOf(synthetic as AiChatMessage)).toBe(
      replay.findIndex((m) =>
        (m.toolCalls ?? []).some((c) => c.id === "fix-1"),
      ) + 1,
    );
  });
});
