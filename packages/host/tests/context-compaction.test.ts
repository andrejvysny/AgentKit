/**
 * Agent-session compaction (ADR 0017 part 2) end to end over the fake store:
 * threshold and manual triggers, summary reuse, invalidation after a branch
 * switch / fork / edit, and the fall back to trimming when the summarizer fails.
 */
import { describe, expect, it } from "bun:test";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";
import type { AiChatRequest, AiTool } from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
import {
  AgentSessionService,
  COMPACT_REQUESTED_METADATA_KEY,
  SESSION_METADATA_KEY,
  SUMMARIZATION_SYSTEM_PROMPT,
  TaskService,
  TurnRunner,
  createUserInputContributor,
  type ToolSetContributor,
} from "../src/index.js";
import { MemoryContextSummaryStore } from "../../adapters-memory/src/memory-context-summary-store.js";
import {
  FakeAgentInputStore,
  createHarness,
  type TestHarness,
} from "./fakes.js";

class RoutingProvider extends MockProviderClient {
  readonly requests: AiChatMessage[][] = [];
  readonly summaryRequests: AiChatMessage[][] = [];
  readonly summarizer = new MockProviderClient();
  failSummary = false;
  override streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    if (input.messages[0]?.content === SUMMARIZATION_SYSTEM_PROMPT) {
      this.summaryRequests.push(structuredClone(input.messages));
      if (this.failSummary) throw new Error("provider down: secret text");
      return this.summarizer.streamChat(input);
    }
    this.requests.push(structuredClone(input.messages));
    return super.streamChat(input);
  }
}

interface Fixture extends TestHarness {
  runner: TurnRunner;
  mock: RoutingProvider;
  service: AgentSessionService;
  summaries: MemoryContextSummaryStore;
  chatId: string;
}

const BIG_FILE = `FILE-HEAD ${"y".repeat(50_000)} FILE-TAIL`;

const SMALL_WINDOW = {
  context: { windowTokens: 4000, reserveTokens: 1000, keepRecentTokens: 1000 },
};

async function setup(
  options: {
    session?: Record<string, unknown> | false;
    /** Adds a 50 KB `read_file` and the human-input tool. */
    extras?: boolean;
  } = {},
): Promise<Fixture> {
  const session = options.session ?? SMALL_WINDOW;
  const harness = createHarness();
  const mock = new RoutingProvider();
  mock.summarizer.emitUsage = true;
  mock.summarizer.setScript([
    { steps: [{ kind: "text", content: "SUMMARY-ONE" }] },
    { steps: [{ kind: "text", content: "SUMMARY-TWO" }] },
    { steps: [{ kind: "text", content: "SUMMARY-THREE" }] },
  ]);
  const echo: AiTool<{ text?: string }, Record<string, never>> = {
    definition: {
      name: "echo",
      version: "1.0.0",
      effect: "read",
      capability: "echo",
      description: "Echo the input.",
      inputSchema: { type: "object", properties: { text: { type: "string" } } },
    },
    async execute(ctx) {
      return {
        ok: true as const,
        data: {},
        sources: [],
        warnings: [],
        truncated: false,
        limits: (ctx as unknown as { limits: never }).limits,
      };
    },
  };
  const readFile: AiTool<{ path?: string }, { text: string }> = {
    definition: {
      name: "read_file",
      version: "1.0.0",
      effect: "read",
      capability: "read_file",
      description: "Read a file.",
      inputSchema: { type: "object", properties: { path: { type: "string" } } },
    },
    async execute(ctx) {
      return {
        ok: true as const,
        data: { text: BIG_FILE },
        sources: [],
        warnings: [],
        truncated: false,
        limits: (ctx as unknown as { limits: never }).limits,
      };
    },
  };
  const contributor: ToolSetContributor = {
    namespace: "test",
    contribute: async () =>
      options.extras === true
        ? [echo as AiTool, readFile as AiTool]
        : [echo as AiTool],
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
  harness.store.inputs = new FakeAgentInputStore(harness.clock);
  const summaries = new MemoryContextSummaryStore({
    withGate: async (operation) => operation(),
  });
  harness.store.summaries = summaries;
  const chat = await harness.store.conversations.createChat({
    id: "chat-1",
    ...(session === false
      ? {}
      : {
          metadata: {
            [SESSION_METADATA_KEY]: {
              version: 1,
              profile: { id: "default" },
              providerId: "p1",
              model: "m1",
              createdAt: "2026-01-01T00:00:00.000Z",
              ...session,
            },
          },
        }),
  });
  const runner = new TurnRunner({
    store: harness.store,
    taskRunner: harness.taskRunner,
    providerFactory: () => mock,
    contributors:
      options.extras === true
        ? [contributor, createUserInputContributor({ clock: harness.clock })]
        : [contributor],
    clock: harness.clock,
    ids: harness.ids,
  });
  const tasks = new TaskService({
    store: harness.store,
    taskRunner: harness.taskRunner,
    ids: harness.ids,
    clock: harness.clock,
  });
  const service = new AgentSessionService({
    store: harness.store,
    turns: runner,
    tasks,
    taskRunner: harness.taskRunner,
    clock: harness.clock,
    ids: harness.ids,
  });
  return {
    ...harness,
    runner,
    mock,
    service,
    summaries,
    chatId: chat.id,
  };
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

interface Turn {
  runId: string;
  assistantMessageId: string;
}

async function turn(
  f: Fixture,
  content: string,
  options: { chatId?: string; parent?: Turn } = {},
): Promise<Turn> {
  const { runId, assistantMessageId } = await f.runner.submitMessage({
    chatId: options.chatId ?? f.chatId,
    content,
    ...(options.parent === undefined
      ? {}
      : { parentMessageId: options.parent.assistantMessageId }),
  });
  await drive(f, runId);
  return { runId, assistantMessageId };
}

const big = (label: string) => `${label} ${"x".repeat(2400)}`;

/** One tool call then an answer, per turn. */
function scripts(count: number) {
  return Array.from({ length: count }, (_, i) => [
    {
      steps: [
        {
          kind: "tool_call" as const,
          toolCallId: `call-${i + 1}`,
          name: "echo",
          argumentsJson: `{"text":"${i + 1}"}`,
        },
      ],
    },
    { steps: [{ kind: "text" as const, content: `answer ${i + 1}` }] },
  ]).flat();
}

async function events(f: Fixture, runId: string): Promise<AiRunEvent[]> {
  return (await f.store.tasks.listEvents(runId)) as AiRunEvent[];
}

async function warningCodes(f: Fixture, runId: string): Promise<string[]> {
  return (await events(f, runId)).flatMap((event) =>
    event.type === "run.warning" ? [event.data.code] : [],
  );
}

function labels(messages: readonly AiChatMessage[]): string[] {
  return messages.map((message) => {
    const text =
      typeof message.content === "string" ? message.content : "[parts]";
    if (message.role === "tool") return `tool:${message.toolCallId}`;
    if (message.toolCalls?.length) return "assistant:calls";
    return `${message.role}:${text.slice(0, 18)}`;
  });
}

/** Four big turns leave the fifth over the 4000-token window's budget. */
async function fillFourTurns(f: Fixture): Promise<Turn> {
  let prev: Turn | undefined;
  for (const n of [1, 2, 3, 4])
    prev = await turn(f, big(`q${n}`), prev ? { parent: prev } : {});
  return prev as Turn;
}

const SUMMARY_PREFIX =
  "The conversation history before this point was compacted into the following summary:";

describe("threshold compaction", () => {
  it("summarizes older turns, sends summary + kept tail + current turn, and saves it", async () => {
    const f = await setup();
    f.mock.setScript(scripts(6));
    await fillFourTurns(f);
    expect(f.mock.summaryRequests).toHaveLength(0);

    const fifth = await turn(f, big("q5"));
    expect(f.mock.summaryRequests).toHaveLength(1);
    const saved = await f.summaries.listSummaries(f.chatId);
    expect(saved).toHaveLength(1);
    expect(saved[0]?.summary).toContain("SUMMARY-ONE");

    const compacted = (await events(f, fifth.runId)).find(
      (event) => event.type === "run.context.compacted",
    );
    expect(
      compacted?.type === "run.context.compacted" && compacted.data,
    ).toMatchObject({
      summaryId: saved[0]?.summaryId,
      reason: "threshold",
    });

    const request = f.mock.requests.at(-2) ?? [];
    const shape = labels(request);
    expect(shape[0]).toBe(`user:${SUMMARY_PREFIX.slice(0, 18)}`);
    expect(String(request[0]?.content)).toContain("SUMMARY-ONE");
    expect(shape.at(-1)).toBe(`user:${big("q5").slice(0, 18)}`);
    expect(shape).not.toContain(`user:${big("q1").slice(0, 18)}`);
    expect(shape).toHaveLength(2);
  });

  it("appends the compaction's usage through the host event path with contiguous seq", async () => {
    const f = await setup();
    f.mock.setScript(scripts(6));
    await fillFourTurns(f);
    const fifth = await turn(f, big("q5"));
    const log = (await f.store.tasks.listEvents(fifth.runId)) as AiRunEvent[];
    const usage = log.filter((event) => event.type === "run.usage");
    expect(usage.length).toBeGreaterThan(0);
    expect(usage[0]?.type === "run.usage" && usage[0].data.step).toBe(0);
    const compactedAt = log.findIndex(
      (event) => event.type === "run.context.compacted",
    );
    expect(compactedAt).toBeGreaterThanOrEqual(0);
    expect(compactedAt).toBeLessThan(
      log.findIndex((event) => event.type === "run.started"),
    );
    log.forEach((event, index) => {
      expect(event.seq).toBe((log[0]?.seq ?? 0) + index);
    });
  });

  it("reuses the saved summary on the next turn without compacting again", async () => {
    const f = await setup();
    f.mock.setScript(scripts(7));
    await fillFourTurns(f);
    await turn(f, big("q5"));
    const sixth = await turn(f, "q6 small");
    expect(f.mock.summaryRequests).toHaveLength(1);
    expect(await f.summaries.listSummaries(f.chatId)).toHaveLength(1);
    expect(
      (await events(f, sixth.runId)).some(
        (event) => event.type === "run.context.compacted",
      ),
    ).toBe(false);
    const request = f.mock.requests.at(-2) ?? [];
    expect(String(request[0]?.content)).toContain("SUMMARY-ONE");
    expect(labels(request).at(-1)).toBe("user:q6 small");
  });
});

describe("summary invalidation", () => {
  it("ignores a summary after switching to an older branch", async () => {
    const f = await setup();
    f.mock.setScript(scripts(8));
    const four = await fillFourTurns(f);
    await turn(f, big("q5"), { parent: four });
    expect(await f.summaries.listSummaries(f.chatId)).toHaveLength(1);
    // Branch from turn 1: the summary covers records that are not on this path.
    const first = (await f.store.conversations.listMessages(f.chatId)).find(
      (m) => m.role === "assistant" && m.runId !== undefined,
    );
    expect(first).toBeDefined();
    const branch = await turn(f, "branch question", {
      parent: { runId: "", assistantMessageId: first?.id ?? "" },
    });
    const request = f.mock.requests.at(-2) ?? [];
    expect(JSON.stringify(request)).not.toContain(SUMMARY_PREFIX);
    expect(labels(request)).toContain(`user:${big("q1").slice(0, 18)}`);
    expect(f.mock.summaryRequests).toHaveLength(1);
    expect(
      (await events(f, branch.runId)).some(
        (event) => event.type === "run.context.compacted",
      ),
    ).toBe(false);
  });

  it("ignores the source chat's summary in a fork", async () => {
    const f = await setup();
    f.mock.setScript(scripts(8));
    const four = await fillFourTurns(f);
    await turn(f, big("q5"), { parent: four });
    const messages = await f.store.conversations.listMessages(f.chatId);
    const q2 = messages.find(
      (m) => m.role === "user" && String(m.content).startsWith("q2"),
    );
    const { chat } = await f.store.conversations.forkChat(
      f.chatId,
      q2?.id ?? "",
    );
    expect(await f.summaries.listSummaries(chat.id)).toEqual([]);
    await turn(f, "fork question", { chatId: chat.id });
    const request = f.mock.requests.at(-2) ?? [];
    expect(JSON.stringify(request)).not.toContain(SUMMARY_PREFIX);
    expect(f.mock.summaryRequests).toHaveLength(1);
  });

  it("ignores a summary whose covered record was edited", async () => {
    const f = await setup();
    f.mock.setScript(scripts(8));
    const four = await fillFourTurns(f);
    await turn(f, big("q5"), { parent: four });
    const q1 = (await f.store.conversations.listMessages(f.chatId)).find(
      (m) => m.role === "user" && String(m.content).startsWith("q1"),
    );
    await f.store.conversations.updateMessage(q1?.id ?? "", {
      content: big("q1 edited"),
    });
    await turn(f, big("q6"));
    // Stale summary skipped; the over-budget history compacted afresh.
    expect(f.mock.summaryRequests).toHaveLength(2);
    const request = f.mock.requests.at(-2) ?? [];
    const text = JSON.stringify(request);
    expect(text).toContain("SUMMARY-TWO");
    expect(text).not.toContain("SUMMARY-ONE");
    expect(await f.summaries.listSummaries(f.chatId)).toHaveLength(2);
  });
});

describe("compaction failure", () => {
  it("warns, trims, completes the turn and saves nothing", async () => {
    const f = await setup();
    f.mock.setScript(scripts(6));
    f.mock.failSummary = true;
    await fillFourTurns(f);
    const fifth = await turn(f, big("q5"));
    const codes = await warningCodes(f, fifth.runId);
    expect(codes).toContain("compaction_failed");
    expect(codes).toContain("context_trimmed");
    const warning = (await events(f, fifth.runId)).find(
      (event) =>
        event.type === "run.warning" && event.data.code === "compaction_failed",
    );
    expect(JSON.stringify(warning)).not.toContain("secret");
    expect(await f.summaries.listSummaries(f.chatId)).toEqual([]);
    expect((await f.store.tasks.getTask(fifth.runId))?.status).toBe(
      "completed",
    );
    const request = f.mock.requests.at(-2) ?? [];
    expect(JSON.stringify(request)).not.toContain(SUMMARY_PREFIX);
    expect(labels(request).at(-1)).toBe(`user:${big("q5").slice(0, 18)}`);
  });
});

describe("manual compaction", () => {
  const roomy = {
    context: { windowTokens: 40_000, reserveTokens: 1000, keepRecentTokens: 5 },
  };

  it("compacts the next turn below the threshold and clears the flag", async () => {
    const f = await setup({ session: roomy });
    f.mock.setScript(scripts(5));
    let prev: Turn | undefined;
    for (const n of [1, 2])
      prev = await turn(f, `q${n}`, prev ? { parent: prev } : {});
    expect(f.mock.summaryRequests).toHaveLength(0);

    await f.service.compact(f.chatId);
    const flagged = await f.store.conversations.getChat(f.chatId);
    expect(typeof flagged?.metadata[COMPACT_REQUESTED_METADATA_KEY]).toBe(
      "string",
    );

    const next = await turn(f, "q3");
    expect(f.mock.summaryRequests).toHaveLength(1);
    const compacted = (await events(f, next.runId)).find(
      (event) => event.type === "run.context.compacted",
    );
    expect(
      compacted?.type === "run.context.compacted" && compacted.data.reason,
    ).toBe("manual");
    expect(await f.summaries.listSummaries(f.chatId)).toHaveLength(1);
    const cleared = await f.store.conversations.getChat(f.chatId);
    expect(COMPACT_REQUESTED_METADATA_KEY in (cleared?.metadata ?? {})).toBe(
      false,
    );
    expect(SESSION_METADATA_KEY in (cleared?.metadata ?? {})).toBe(true);

    await turn(f, "q4");
    expect(f.mock.summaryRequests).toHaveLength(1);
  });

  it("clears the flag when there is nothing older than the kept tail", async () => {
    const f = await setup({ session: roomy });
    f.mock.setScript(scripts(2));
    await f.service.compact(f.chatId);
    await turn(f, "q1");
    expect(f.mock.summaryRequests).toHaveLength(0);
    const chat = await f.store.conversations.getChat(f.chatId);
    expect(COMPACT_REQUESTED_METADATA_KEY in (chat?.metadata ?? {})).toBe(
      false,
    );
  });

  it("refuses with chat_busy while a turn is live", async () => {
    const f = await setup({ session: roomy });
    await f.runner.submitMessage({ chatId: f.chatId, content: "q1" });
    await expect(f.service.compact(f.chatId)).rejects.toMatchObject({
      code: "chat_busy",
    });
  });
});

describe("non-session chats", () => {
  it("are never compacted, whatever the store offers", async () => {
    const f = await setup({ session: false });
    f.mock.setScript(scripts(6));
    let prev: Turn | undefined;
    for (const n of [1, 2, 3, 4, 5])
      prev = await turn(f, big(`q${n}`), prev ? { parent: prev } : {});
    expect(f.mock.summaryRequests).toHaveLength(0);
    expect(await f.summaries.listSummaries(f.chatId)).toEqual([]);
    expect(labels(f.mock.requests.at(-2) ?? [])).toContain(
      `user:${big("q1").slice(0, 18)}`,
    );
  });
});

/** Claim a re-queued turn the way the task runner does, then run it. */
async function resume(f: Fixture, taskId: string): Promise<void> {
  await f.store.tasks.transitionTask(taskId, ["queued"], "running");
  await drive(f, taskId);
}

const ROOMY = {
  context: { windowTokens: 100_000, reserveTokens: 1000, keepRecentTokens: 5 },
};

describe("tool-result elision", () => {
  const readThenAnswer = [
    {
      steps: [
        {
          kind: "tool_call" as const,
          toolCallId: "read-1",
          name: "read_file",
          argumentsJson: '{"path":"a.ts"}',
        },
      ],
    },
    { steps: [{ kind: "text" as const, content: "answer 1" }] },
  ];

  async function toolRecords(f: Fixture) {
    // Content only: the store flips `active` as the branch advances.
    return f.store.conversations.messages
      .filter((m) => m.role === "tool")
      .map((m) => ({ id: m.id, content: m.content }));
  }

  it("sends an older turn's big result elided and leaves the stored record untouched", async () => {
    const f = await setup({ session: ROOMY, extras: true });
    f.mock.setScript([
      ...readThenAnswer,
      { steps: [{ kind: "text", content: "answer 2" }] },
      { steps: [{ kind: "text", content: "answer 3" }] },
    ]);
    const first = await turn(f, "q1");
    const stored = await toolRecords(f);
    expect(stored).toHaveLength(1);
    const before = JSON.stringify(stored);
    expect(String(stored[0]?.content).length).toBeGreaterThan(6_000);

    const second = await turn(f, "q2", { parent: first });
    expect(JSON.stringify(f.mock.requests.at(-1)).length).toBeGreaterThan(
      6_000,
    );
    await turn(f, "q3", { parent: second });

    const request = f.mock.requests.at(-1) ?? [];
    const sent = request.find((m) => m.role === "tool");
    expect(String(sent?.content)).toContain(
      "characters of an earlier tool result elided",
    );
    expect(String(sent?.content).length).toBeLessThan(4000);
    expect(sent?.toolCallId).toBe("read-1");
    expect(JSON.stringify(await toolRecords(f))).toBe(before);
  });

  it("sends results whole when the session disables elision", async () => {
    const f = await setup({
      session: { context: { ...ROOMY.context, toolResultElision: false } },
      extras: true,
    });
    f.mock.setScript([
      ...readThenAnswer,
      { steps: [{ kind: "text", content: "answer 2" }] },
      { steps: [{ kind: "text", content: "answer 3" }] },
    ]);
    let prev: Turn | undefined;
    for (const q of ["q1", "q2", "q3"])
      prev = await turn(f, q, prev ? { parent: prev } : {});
    const sent = (f.mock.requests.at(-1) ?? []).find((m) => m.role === "tool");
    expect(String(sent?.content).length).toBeGreaterThan(6_000);
    expect(String(sent?.content)).not.toContain("elided");
  });
});

describe("compaction and parked turns", () => {
  const askThenDone = [
    {
      steps: [
        {
          kind: "tool_call" as const,
          toolCallId: "ask-1",
          name: "request_user_input",
          argumentsJson: '{"prompt":"Pick one"}',
        },
      ],
    },
    { steps: [{ kind: "text" as const, content: "resumed answer" }] },
  ];

  it("keeps a pending question and the current turn across compaction", async () => {
    const f = await setup({ session: ROOMY, extras: true });
    f.mock.setScript([...scripts(2), ...askThenDone]);
    let prev: Turn | undefined;
    for (const n of [1, 2])
      prev = await turn(f, `q${n}`, prev ? { parent: prev } : {});
    await f.service.compact(f.chatId);

    const parked = await turn(f, "need a decision", { parent: prev as Turn });
    expect(f.mock.summaryRequests).toHaveLength(1);
    expect(await f.summaries.listSummaries(f.chatId)).toHaveLength(1);

    const state = await f.service.getState(f.chatId);
    expect(state.activeTurn?.status).toBe("waiting_approval");
    expect(state.waiting?.kind).toBe("human_request");
    // A live turn refuses a second compaction request instead of dropping it.
    await expect(f.service.compact(f.chatId)).rejects.toMatchObject({
      code: "chat_busy",
    });
    expect((await f.service.getState(f.chatId)).waiting?.inputId).toBe(
      state.waiting?.inputId,
    );

    await f.service.answer({
      requestId: state.waiting?.inputId ?? "",
      answerId: "a1",
      response: { choice: "b" },
    });
    await resume(f, parked.runId);
    expect((await f.store.tasks.getTask(parked.runId))?.status).toBe(
      "completed",
    );
    const resumed = f.mock.requests.at(-1) ?? [];
    expect(String(resumed[0]?.content)).toContain("SUMMARY-ONE");
    expect(labels(resumed).slice(-3)).toEqual([
      "user:need a decision",
      "assistant:calls",
      "tool:ask-1",
    ]);
    expect(String(resumed.at(-1)?.content)).toContain('"choice":"b"');
  });
});
