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
        previousContent: "   ",
        writeBack: "fix it",
      }).map((m) => m.role),
    ).toEqual(["user"]);
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

  it("sends MINIMAL re-context — system, the last answer, the write-back", async () => {
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
      "assistant",
      "user",
    ]);
    expect(correction[0]?.content).toBe("SYSTEM PROMPT");
    expect(correction[1]?.content).toBe("all set");
    const writeBack = String(correction[2]?.content ?? "");
    expect(writeBack).toContain("- x");
    expect(writeBack).toContain("- y");
    // NOT the full history: no tool turns, and not the original question.
    expect(correction.some((m) => m.role === "tool")).toBe(false);
    expect(correction.some((m) => m.content === "go")).toBe(false);
    expect(
      correction.some(
        (m) => m.toolCalls !== undefined && m.toolCalls.length > 0,
      ),
    ).toBe(false);
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
